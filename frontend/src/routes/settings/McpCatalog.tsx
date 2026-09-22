/**
 * Settings → Tools → Add from catalog (docs/ux.md §10).
 *
 * The official MCP Registry, searched by name, one page at a time. The
 * flow is linear and each page asks one thing: which server (a row is the
 * answer); how it should run, only when the entry offers more than one
 * way; and the values it needs before it can be added — the name it is
 * registered under and whatever the registry says the server wants — on
 * a page whose single primary is `Add server`. A server that needs nothing
 * is added the moment its row is chosen; only a name already in use sends
 * it to the last page, with the clash explained and the name to change.
 *
 * The control plane does the translating (`GET /v1/catalog/mcp-servers`
 * returns entries already reduced to the installs flyco can run and their
 * inputs), so this page knows nothing of the registry's schema: it shows
 * inputs and posts their values back.
 */
import { For, Show, createEffect, createSignal, on, onCleanup } from "solid-js";
import { A, useNavigate } from "@solidjs/router";
import { ArrowLeft } from "lucide-solid";
import SearchField from "../../components/SearchField";
import ProblemNotice from "../../components/ProblemNotice";
import { createQuery } from "../../lib/query";
import { cx } from "../../lib/cx";
import {
  installCatalogMcpServer,
  listCatalogMcpServers,
  type CatalogInstallKind,
  type CatalogMcpInstall,
  type CatalogMcpServer,
} from "../../api/client";
import styles from "./Settings.module.css";

/** Where the flow starts from and returns to. */
const TOOLS = "/settings/tools";

/**
 * How long typing may pause before the registry is asked.
 *
 * Every distinct search is one registry page read (the control plane keeps
 * each for an hour); asking on every keystroke of `playwright` would be ten
 * reads for one answer.
 */
const SEARCH_SETTLE_MS = 250;

const KIND_LABEL: Record<CatalogInstallKind, string> = {
  remote: "Remote",
  npm: "npm",
  pypi: "PyPI",
};

/** The page on screen, and what the pages before it decided. */
type Step =
  | { readonly page: "pick" }
  | { readonly page: "kind"; readonly server: CatalogMcpServer }
  | {
      readonly page: "details";
      readonly server: CatalogMcpServer;
      readonly install: CatalogMcpInstall;
      /** Why a one-click add came here instead: the name was taken. */
      readonly clash: unknown;
    };

/**
 * What an install is, without repeating the kind beside it.
 *
 * The label the control plane sends opens with the kind (`Remote ·
 * mcp.0n.network`) because it is shown on its own elsewhere; under a
 * heading that already says `Remote`, the prefix is noise.
 */
function installDetail(install: CatalogMcpInstall): string {
  const prefix = `${KIND_LABEL[install.kind]} · `;
  return install.label.startsWith(prefix) ? install.label.slice(prefix.length) : install.label;
}

/** What a server is called on screen: its title, else its registry name. */
function displayName(server: CatalogMcpServer): string {
  return server.title ?? server.name;
}

export default function McpCatalog() {
  const navigate = useNavigate();
  const [step, setStep] = createSignal<Step>({ page: "pick" });
  const [adding, setAdding] = createSignal<string | null>(null);
  const [failure, setFailure] = createSignal<unknown>(null);

  function done(): void {
    navigate(TOOLS);
  }

  /**
   * Registers one install with the answers given, or takes the flow to the
   * details page when the name is already in use.
   */
  async function add(
    server: CatalogMcpServer,
    install: CatalogMcpInstall,
    name: string,
    values: Record<string, string>,
  ): Promise<void> {
    setAdding(server.name);
    setFailure(null);
    try {
      await installCatalogMcpServer({ server: server.name, kind: install.kind, name, values });
      done();
    } catch (error) {
      if (step().page === "details") {
        setFailure(error);
      } else {
        setStep({ page: "details", server, install, clash: error });
      }
    } finally {
      setAdding(null);
    }
  }

  /** The row was chosen: the next question, or the add itself. */
  function choose(server: CatalogMcpServer): void {
    const [only] = server.installs;
    if (only === undefined || server.installs.length > 1) {
      setStep({ page: "kind", server });
      return;
    }
    chooseInstall(server, only);
  }

  function chooseInstall(server: CatalogMcpServer, install: CatalogMcpInstall): void {
    if (install.inputs.length === 0) {
      void add(server, install, server.suggested_name, {});
      return;
    }
    setStep({ page: "details", server, install, clash: null });
  }

  return (
    <section class={styles.section}>
      <Show when={step().page === "pick"}>
        <PickPage
          adding={adding()}
          failure={failure()}
          onChoose={choose}
        />
      </Show>
      <Show when={step().page === "kind" ? step() : null}>
        {(current) => (
          <KindPage
            server={(current() as Extract<Step, { page: "kind" }>).server}
            adding={adding()}
            failure={failure()}
            onBack={() => setStep({ page: "pick" })}
            onChoose={(install) =>
              chooseInstall((current() as Extract<Step, { page: "kind" }>).server, install)
            }
          />
        )}
      </Show>
      <Show when={step().page === "details" ? step() : null}>
        {(current) => {
          const details = () => current() as Extract<Step, { page: "details" }>;
          return (
            <DetailsPage
              server={details().server}
              install={details().install}
              clash={details().clash}
              failure={failure()}
              busy={adding() !== null}
              onBack={() =>
                setStep(
                  details().server.installs.length > 1
                    ? { page: "kind", server: details().server }
                    : { page: "pick" },
                )
              }
              onSubmit={(name, values) => void add(details().server, details().install, name, values)}
            />
          );
        }}
      </Show>
    </section>
  );
}

/* ── Page 1: which server ─────────────────────────────────────────────── */

function PickPage(props: {
  adding: string | null;
  failure: unknown;
  onChoose: (server: CatalogMcpServer) => void;
}) {
  const [typed, setTyped] = createSignal("");
  const [search, setSearch] = createSignal("");
  const [cursor, setCursor] = createSignal<string | null>(null);
  const [listed, setListed] = createSignal<CatalogMcpServer[]>([]);
  const [nextCursor, setNextCursor] = createSignal<string | null>(null);

  // The search the registry is asked for settles a moment after typing
  // stops, and a new search starts the listing over from its first page.
  let settle: ReturnType<typeof setTimeout> | undefined;
  createEffect(
    on(typed, (value) => {
      clearTimeout(settle);
      settle = setTimeout(() => {
        setListed([]);
        setCursor(null);
        setSearch(value.trim());
      }, SEARCH_SETTLE_MS);
    }, { defer: true }),
  );
  onCleanup(() => clearTimeout(settle));

  const [page, { refetch }] = createQuery(
    () => ({ search: search(), cursor: cursor() }),
    ({ search, cursor }) =>
      listCatalogMcpServers({
        search: search === "" ? undefined : search,
        cursor: cursor ?? undefined,
      }),
  );

  // A first page replaces the listing; a `Load more` page extends it.
  createEffect(
    on(page, (loaded) => {
      if (loaded === undefined) {
        return;
      }
      setListed((known) => (cursor() === null ? loaded.servers : [...known, ...loaded.servers]));
      setNextCursor(loaded.next_cursor ?? null);
    }),
  );

  return (
    <>
      <header class={styles.sectionHead}>
        <A href={TOOLS} class={styles.backLink}>
          <ArrowLeft size={14} aria-hidden="true" />
          Back to Tools
        </A>
        <h2>Add an MCP server</h2>
      </header>

      <SearchField
        placeholder="Search the registry"
        aria-label="Search the registry"
        value={typed()}
        onInput={(event) => setTyped(event.currentTarget.value)}
      />

      <ProblemNotice error={props.failure} />
      <ProblemNotice error={page.error} action={{ label: "Retry", onClick: () => void refetch() }} />

      <Show
        when={listed().length > 0 || page.loading}
        fallback={
          <p class={styles.note}>
            <Show when={page.error === undefined}>Nothing in the registry matches “{search()}”.</Show>
          </p>
        }
      >
        <ul class={styles.rows} aria-label="Servers">
          <For each={listed()}>
            {(server) => (
              <li>
                <button
                  type="button"
                  class={styles.row}
                  disabled={props.adding !== null}
                  aria-busy={props.adding === server.name}
                  onClick={() => props.onChoose(server)}
                >
                  <span class={styles.rowBody}>
                    <span class={styles.rowTitle}>{displayName(server)}</span>
                    <span class={styles.rowMeta}>{server.description}</span>
                    <span class={styles.mono}>{server.name}</span>
                  </span>
                  <span class={styles.rowKinds}>
                    <For each={server.installs}>
                      {(install) => <span class={styles.kind}>{KIND_LABEL[install.kind]}</span>}
                    </For>
                  </span>
                  <Show when={props.adding === server.name}>
                    <span class={styles.note}>Adding…</span>
                  </Show>
                </button>
              </li>
            )}
          </For>
        </ul>
        <Show when={page.loading}>
          <p class={styles.note}>Reading the registry…</p>
        </Show>
        <Show when={nextCursor() !== null && !page.loading}>
          <div>
            <button type="button" class={styles.pill} onClick={() => setCursor(nextCursor())}>
              Load more
            </button>
          </div>
        </Show>
      </Show>
    </>
  );
}

/* ── Page 2: how it runs, when there is a choice ──────────────────────── */

function KindPage(props: {
  server: CatalogMcpServer;
  adding: string | null;
  failure: unknown;
  onBack: () => void;
  onChoose: (install: CatalogMcpInstall) => void;
}) {
  return (
    <>
      <header class={styles.sectionHead}>
        <button type="button" class={cx(styles.backLink, styles.backButton)} onClick={props.onBack}>
          <ArrowLeft size={14} aria-hidden="true" />
          Back
        </button>
        <h2>{displayName(props.server)}</h2>
        <p class={styles.lede}>How should it run?</p>
      </header>
      <ProblemNotice error={props.failure} />
      <ul class={styles.rows} aria-label="Ways to run it">
        <For each={props.server.installs}>
          {(install) => (
            <li>
              <button
                type="button"
                class={styles.row}
                disabled={props.adding !== null}
                onClick={() => props.onChoose(install)}
              >
                <span class={styles.rowBody}>
                  <span class={styles.rowTitle}>{KIND_LABEL[install.kind]}</span>
                  <span class={styles.rowMeta}>{installDetail(install)}</span>
                </span>
                <Show when={install.inputs.length > 0}>
                  <span class={styles.note}>
                    {install.inputs.length === 1
                      ? "1 value to fill"
                      : `${install.inputs.length} values to fill`}
                  </span>
                </Show>
              </button>
            </li>
          )}
        </For>
      </ul>
    </>
  );
}

/* ── Page 3: the name and the values ──────────────────────────────────── */

function DetailsPage(props: {
  server: CatalogMcpServer;
  install: CatalogMcpInstall;
  clash: unknown;
  failure: unknown;
  busy: boolean;
  onBack: () => void;
  onSubmit: (name: string, values: Record<string, string>) => void;
}) {
  const [name, setName] = createSignal(props.server.suggested_name);
  const [values, setValues] = createSignal<Record<string, string>>(
    Object.fromEntries(
      props.install.inputs.map((input) => [input.key, input.default ?? ""] as const),
    ),
  );

  function onSubmit(event: SubmitEvent): void {
    event.preventDefault();
    props.onSubmit(name(), values());
  }

  return (
    <>
      <header class={styles.sectionHead}>
        <button type="button" class={cx(styles.backLink, styles.backButton)} onClick={props.onBack}>
          <ArrowLeft size={14} aria-hidden="true" />
          Back
        </button>
        <h2>{displayName(props.server)}</h2>
        <p class={styles.lede}>{props.install.label}</p>
      </header>

      <form class={styles.form} onSubmit={onSubmit}>
        <div class={styles.field}>
          <label for="catalog-name">Name</label>
          <input
            id="catalog-name"
            value={name()}
            onInput={(event) => setName(event.currentTarget.value)}
            required
            pattern="[A-Za-z0-9_\-]+"
            title="Letters, digits, dashes and underscores"
          />
          <p class={styles.hint}>What sessions announce the server as.</p>
        </div>
        <For each={props.install.inputs}>
          {(input) => (
            <div class={styles.field}>
              <label for={`catalog-${input.key}`}>
                {input.label}
                <Show when={!input.required}> (optional)</Show>
              </label>
              <input
                id={`catalog-${input.key}`}
                type={input.secret ? "password" : "text"}
                autocomplete={input.secret ? "off" : undefined}
                value={values()[input.key] ?? ""}
                onInput={(event) =>
                  setValues((known) => ({ ...known, [input.key]: event.currentTarget.value }))
                }
                required={input.required}
              />
              <Show when={input.description}>
                {(description) => <p class={styles.hint}>{description()}</p>}
              </Show>
            </div>
          )}
        </For>

        <ProblemNotice error={props.clash} />
        <ProblemNotice error={props.failure} />
        <div class={styles.formActions}>
          <button type="submit" class={styles.pillPrimary} disabled={props.busy}>
            {props.busy ? "Adding…" : "Add server"}
          </button>
        </div>
      </form>
    </>
  );
}
