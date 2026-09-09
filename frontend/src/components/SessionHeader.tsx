/**
 * The session header (docs/ux.md §9.1).
 *
 * One quiet row: the title, editable in place; what the session works on;
 * and a `⋯` menu for everything that is not the conversation. Nothing else.
 * The header used to carry a status pill, two rings, the machine's SKU and
 * an `Archive` button, and it read as a dashboard over a chat; every one of
 * those facts now lives where it is acted on — the composer's own row, the
 * transcript, the state notice — which is where the official apps keep
 * them, and where a reader's eye already is.
 *
 * The title is edited where it is shown rather than in a dialog, because
 * renaming a session is a one-word change and a dialog would be three
 * clicks around it. Escape abandons the edit; Enter and blur commit it.
 */
import { Show, createSignal } from "solid-js";
import {
  Archive,
  Copy,
  MoreHorizontal,
  PanelRightClose,
  PanelRightOpen,
  Pencil,
  Play,
  Scaling,
  SlidersHorizontal,
  Square,
} from "lucide-solid";
import Popover from "./Popover";
import type { MachineView, SessionDetail } from "../api/client";
import type { ConnectionState } from "../api/relay";
import { MACHINE_STATE_LABEL } from "../lib/machines";
import { PROVIDER_LABEL } from "../lib/providers";
import styles from "./SessionHeader.module.css";

/**
 * What a socket that is not carrying events is called.
 *
 * Only the unhappy states have a label. A live relay says nothing: "Live"
 * on a working page is chrome telling the user what they can already see,
 * and the point of this readout is to explain a page that has gone quiet.
 */
const CONNECTION_LABEL: Partial<Record<ConnectionState, string>> = {
  connecting: "Connecting…",
  reconnecting: "Reconnecting…",
  closed: "Disconnected",
  // `failed` has no label: the relay stopped for a reason the page states
  // in full, in a notice with a way out of it, and a second readout saying
  // `Disconnected` beside it would only be a quieter version of the same
  // sentence (issue #137).
};

export interface SessionHeaderProps {
  session: SessionDetail | undefined;
  /** The id, for the `Copy session id` action; never shown as a title. */
  sessionId: string;
  /** The relay socket's state, shown only while it is not carrying events. */
  connection: ConnectionState;
  /** The machine, for the menu's start/stop and its footer line. */
  machine: MachineView | undefined;
  /** Renames the session. */
  onRename: (title: string) => void;
  onArchive: () => void;
  archiving: boolean;
  onStartMachine: () => void;
  onStopMachine: () => void;
  /** Whether the drawer of docs/ux.md §9.4 is open. */
  drawerOpen: boolean;
  /** Opens or closes it; the header holds the toggle, the page holds the state. */
  onToggleDrawer: () => void;
  /**
   * Opens the drawer on a given tab, for the menu items that live there.
   *
   * `Resize` asks for the resize control itself rather than for the tab it
   * is on: a menu item that opened a panel and left the user to find the
   * button did half of what it said (issue #138).
   */
  onOpenPanel: (request: { panel: "machine" | "env"; resize?: boolean }) => void;
}

export default function SessionHeader(props: SessionHeaderProps) {
  const [editing, setEditing] = createSignal(false);
  const [draft, setDraft] = createSignal("");
  const [copied, setCopied] = createSignal(false);

  const title = () => props.session?.title ?? "";

  function beginEdit(): void {
    setDraft(title());
    setEditing(true);
  }

  function commit(): void {
    const next = draft().trim();
    setEditing(false);
    if (next !== "" && next !== title()) {
      props.onRename(next);
    }
  }

  function copyId(): void {
    void navigator.clipboard.writeText(props.sessionId).then(
      () => setCopied(true),
      () => setCopied(false),
    );
  }

  return (
    <header class={styles.header}>
      <div class={styles.identity}>
        {/*
          Before the session has loaded there is no title to show, and the
          id is not one: a UUID where a name goes reads as an error. A quiet
          block the width of a title holds the place instead.
        */}
        <Show
          when={editing()}
          fallback={
            <Show
              when={props.session}
              fallback={<span class={styles.titleSkeleton} aria-label="Loading the session" />}
            >
              <button type="button" class={styles.title} onClick={beginEdit} title="Rename">
                {title()}
              </button>
            </Show>
          }
        >
          <input
            class={styles.titleInput}
            value={draft()}
            aria-label="Session title"
            autofocus
            onInput={(event) => setDraft(event.currentTarget.value)}
            onBlur={commit}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                event.preventDefault();
                event.currentTarget.blur();
              }
              if (event.key === "Escape") {
                event.preventDefault();
                setEditing(false);
              }
            }}
          />
        </Show>
        <Show when={props.session}>
          {/*
            `repo · branch` in docs/ux.md §9.1. A session opened before flyco
            recorded branches has none, and the repository stands alone for
            those: a placeholder would be a claim about somebody's checkout,
            and an em dash would be a claim that there is no branch.
          */}
          {(session) => (
            <span class={styles.repo}>
              {session().repo}
              <Show when={session().branch}>
                {(branch) => <span class={styles.branch}>· {branch()}</span>}
              </Show>
            </span>
          )}
        </Show>
      </div>

      <div class={styles.actions}>
        {/*
          The one readout the header keeps, and only while it is bad news:
          a page that has gone quiet owes the reader the reason, and the
          socket is the one fact nothing else on the page can show.
        */}
        <Show when={CONNECTION_LABEL[props.connection]}>
          {(label) => <span class={styles.connection}>{label()}</span>}
        </Show>

        {/*
          The drawer's toggle lives here, beside the menu, the way the
          official apps keep their side-panel toggle in the title bar: a
          handle at the top of the transcript column sat exactly where the
          first user message lands and read as an avatar on it.
        */}
        <button
          type="button"
          class={styles.iconAction}
          aria-expanded={props.drawerOpen}
          title={props.drawerOpen ? "Hide the panel (⌘.)" : "Terminal, files and the machine (⌘.)"}
          aria-label={props.drawerOpen ? "Hide the panel" : "Show the panel"}
          onClick={() => props.onToggleDrawer()}
        >
          <Show when={props.drawerOpen} fallback={<PanelRightOpen size={16} aria-hidden="true" />}>
            <PanelRightClose size={16} aria-hidden="true" />
          </Show>
        </button>

        <Popover
          label="Session actions"
          align="end"
          trigger={(attrs) => (
            <button {...attrs} type="button" class={styles.iconAction} aria-label="Session actions">
              <MoreHorizontal size={16} aria-hidden="true" />
            </button>
          )}
        >
          {(close) => (
            <ul class={styles.menu}>
              <li>
                <button
                  type="button"
                  class={styles.menuItem}
                  disabled={props.session === undefined}
                  onClick={() => {
                    close();
                    beginEdit();
                  }}
                >
                  <Pencil size={14} aria-hidden="true" />
                  Rename
                </button>
              </li>
              <Show when={props.session?.state !== "archived"}>
                <li>
                  <button
                    type="button"
                    class={styles.menuItem}
                    disabled={props.archiving || props.session === undefined}
                    onClick={() => {
                      props.onArchive();
                      close();
                    }}
                  >
                    <Archive size={14} aria-hidden="true" />
                    {props.archiving ? "Archiving…" : "Archive"}
                  </button>
                </li>
              </Show>
              <li class={styles.menuDivider} role="presentation" />
              <li>
                <button
                  type="button"
                  class={styles.menuItem}
                  disabled={props.machine?.state === "running"}
                  onClick={() => {
                    props.onStartMachine();
                    close();
                  }}
                >
                  <Play size={14} aria-hidden="true" />
                  Start machine
                </button>
              </li>
              <li>
                <button
                  type="button"
                  class={styles.menuItem}
                  disabled={props.machine !== undefined && props.machine.state !== "running"}
                  onClick={() => {
                    props.onStopMachine();
                    close();
                  }}
                >
                  <Square size={14} aria-hidden="true" />
                  Stop machine
                </button>
              </li>
              <li>
                <button
                  type="button"
                  class={styles.menuItem}
                  onClick={() => {
                    props.onOpenPanel({ panel: "machine", resize: true });
                    close();
                  }}
                >
                  <Scaling size={14} aria-hidden="true" />
                  Resize
                </button>
              </li>
              <li>
                <button
                  type="button"
                  class={styles.menuItem}
                  onClick={() => {
                    props.onOpenPanel({ panel: "env" });
                    close();
                  }}
                >
                  <SlidersHorizontal size={14} aria-hidden="true" />
                  Edit .env
                </button>
              </li>
              <li>
                <button type="button" class={styles.menuItem} onClick={copyId}>
                  <Copy size={14} aria-hidden="true" />
                  {copied() ? "Copied session id" : "Copy session id"}
                </button>
              </li>
              <Show when={props.machine}>
                {(machine) => (
                  <li class={styles.menuFooter}>
                    {PROVIDER_LABEL[machine().spec.provider]} · {machine().region} ·{" "}
                    {MACHINE_STATE_LABEL[machine().state]}
                  </li>
                )}
              </Show>
            </ul>
          )}
        </Popover>
      </div>
    </header>
  );
}
