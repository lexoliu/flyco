/**
 * The session's right-hand drawer (docs/ux.md §9.4).
 *
 * Collapsed by default, and that is the point: the terminal, the files, the
 * diff, the machine and the `.env` are all things a user occasionally needs
 * and never reads while following a conversation. The page that showed them
 * all at once made the transcript — the thing the product is for — the
 * narrower half of the screen.
 *
 * `Files` and `Diff` are two tabs rather than one because they answer two
 * questions: what is on the disk, and what did the agent change. Both are
 * read live from the machine, and so is the terminal — with no machine
 * there is nothing behind those tabs, so the tabs grey out and selection
 * moves to one that still answers. `Env` stays lit: the `.env` lives in
 * the control plane. The machine itself is not a tab — its panel hangs
 * off the composer's machine chip, where the readout naming it is.
 *
 * `⌘.` (`Ctrl+.` off macOS) toggles it, which is the one keyboard shortcut
 * on the page. On a phone there is no room beside the transcript, so the
 * drawer covers it instead and carries its own close button; the header's
 * toggle is behind it.
 */
import { For, Match, Show, Switch, createEffect, createSignal, on, onCleanup } from "solid-js";
import { FileCode2, GitCompare, SlidersHorizontal, TerminalSquare, X } from "lucide-solid";
import DiffPanel from "./DiffPanel";
import EnvEditor from "./EnvEditor";
import FilesPanel from "./FilesPanel";
import TerminalPanel from "./terminal/TerminalPanel";
import type { SessionRelay } from "../api/relay";
import { cx } from "../lib/cx";
import styles from "./SessionDrawer.module.css";

type Tab = "terminal" | "files" | "diff" | "env";

const TABS: readonly { id: Tab; label: string; needsMachine?: boolean }[] = [
  { id: "terminal", label: "Terminal", needsMachine: true },
  { id: "files", label: "Files", needsMachine: true },
  { id: "diff", label: "Diff", needsMachine: true },
  { id: "env", label: "Env" },
];

export interface SessionDrawerProps {
  sessionId: string;
  /**
   * Whether the drawer is open.
   *
   * Owned by the page rather than by the drawer, because the control that
   * opens it lives in the header (docs/ux.md §9.1): a toggle beside the
   * panel it opens sat at the top of the transcript column, right where
   * the first user message lands, and read as an avatar on it.
   */
  open: boolean;
  onOpenChange: (open: boolean) => void;
  relay: SessionRelay;
  /**
   * Whether the session's daemon is there to take what the panes send.
   *
   * The terminal needs it: a keystroke and a resize are delivered or they
   * are nothing, and a pane that took them while the machine was off
   * would be a shell that looked alive and was not.
   */
  machineUp: boolean;
  /** Latest `repo_dirty` summary from the relay, when one has arrived. */
  liveRepoSummary: string | null;
  /**
   * A request to open the drawer on the `.env` tab, from the header's `⋯`
   * menu.
   *
   * Carries the instant it was made so that asking twice is two requests:
   * without it, a user who closed the drawer and picked `Edit .env` again
   * would set an unchanged signal and see nothing happen.
   */
  openEnv?: number | undefined;
}

export default function SessionDrawer(props: SessionDrawerProps) {
  const open = () => props.open;
  const [tab, setTab] = createSignal<Tab>("terminal");
  /**
   * A disabled tab cannot be the selected one: when the machine leaves,
   * the tab that was open on it yields to the one that still answers.
   */
  createEffect(() => {
    if (
      props.machineUp === false &&
      TABS.find((entry) => entry.id === tab())?.needsMachine === true
    ) {
      setTab("env");
    }
  });

  createEffect(
    on(
      () => props.openEnv,
      (at) => {
        if (at !== undefined) {
          setTab("env");
          props.onOpenChange(true);
        }
      },
    ),
  );

  createEffect(() => {
    function onKeyDown(event: KeyboardEvent): void {
      if (event.key === "." && (event.metaKey || event.ctrlKey)) {
        event.preventDefault();
        props.onOpenChange(!props.open);
      }
    }
    document.addEventListener("keydown", onKeyDown);
    onCleanup(() => document.removeEventListener("keydown", onKeyDown));
  });

  return (
    <Show when={open()}>
      <div class={cx(styles.drawer, styles.drawerOpen)}>
        <div class={styles.panel}>
          <div class={styles.tabs} role="tablist" aria-label="Session panels">
            <For each={TABS}>
              {(entry) => (
                <button
                  type="button"
                  role="tab"
                  aria-selected={tab() === entry.id}
                  aria-disabled={(entry.needsMachine === true && !props.machineUp) || undefined}
                  disabled={entry.needsMachine === true && !props.machineUp}
                  title={
                    entry.needsMachine === true && !props.machineUp
                      ? "The machine is not connected"
                      : undefined
                  }
                  class={cx(
                    styles.tab,
                    tab() === entry.id && styles.tabActive,
                    entry.needsMachine === true && !props.machineUp && styles.tabDisabled,
                  )}
                  onClick={() => setTab(entry.id)}
                >
                  <Switch>
                    <Match when={entry.id === "terminal"}>
                      <TerminalSquare size={13} aria-hidden="true" />
                    </Match>
                    <Match when={entry.id === "files"}>
                      <FileCode2 size={13} aria-hidden="true" />
                    </Match>
                    <Match when={entry.id === "diff"}>
                      <GitCompare size={13} aria-hidden="true" />
                    </Match>
                    <Match when={entry.id === "env"}>
                      <SlidersHorizontal size={13} aria-hidden="true" />
                    </Match>
                  </Switch>
                  {entry.label}
                </button>
              )}
            </For>
            <button
              type="button"
              class={styles.close}
              aria-label="Close the panel"
              onClick={() => props.onOpenChange(false)}
            >
              <X size={16} aria-hidden="true" />
            </button>
          </div>

          <div class={styles.body} role="tabpanel">
            <Switch>
              <Match when={tab() === "terminal"}>
                <TerminalPanel sessionId={props.sessionId} relay={props.relay} />
              </Match>
              <Match when={tab() === "files"}>
                <FilesPanel sessionId={props.sessionId} />
              </Match>
              <Match when={tab() === "diff"}>
                <DiffPanel
                  sessionId={props.sessionId}
                  liveRepoSummary={props.liveRepoSummary}
                />
              </Match>
              <Match when={tab() === "env"}>
                <EnvEditor sessionId={props.sessionId} />
              </Match>
            </Switch>
          </div>
        </div>
      </div>
    </Show>
  );
}
