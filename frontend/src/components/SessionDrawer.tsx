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
 * read live from the machine, so both say plainly when there is no machine
 * to read.
 *
 * `⌘.` (`Ctrl+.` off macOS) toggles it, which is the one keyboard shortcut
 * on the page.
 */
import { For, Match, Show, Switch, createEffect, createSignal, onCleanup } from "solid-js";
import {
  Cpu,
  FileCode2,
  GitCompare,
  PanelRightClose,
  PanelRightOpen,
  SlidersHorizontal,
  TerminalSquare,
} from "lucide-solid";
import DiffPanel from "./DiffPanel";
import EnvEditor from "./EnvEditor";
import FilesPanel from "./FilesPanel";
import MachinePanel from "./MachinePanel";
import TerminalPanel from "./terminal/TerminalPanel";
import type { SessionRelay } from "../api/relay";
import { cx } from "../lib/cx";
import styles from "./SessionDrawer.module.css";

type Tab = "terminal" | "files" | "diff" | "machine" | "env";

const TABS: readonly { id: Tab; label: string }[] = [
  { id: "terminal", label: "Terminal" },
  { id: "files", label: "Files" },
  { id: "diff", label: "Diff" },
  { id: "machine", label: "Machine" },
  { id: "env", label: "Env" },
];

export interface SessionDrawerProps {
  sessionId: string;
  relay: SessionRelay;
  /** Latest `repo_dirty` summary from the relay, when one has arrived. */
  liveRepoSummary: string | null;
  /**
   * A request to open the drawer on one tab, from the header's `⋯` menu or
   * a `/resize` command.
   *
   * Carries the instant it was made so that asking for the same tab twice
   * is two requests: without it, a user who closed the drawer and picked
   * `Edit .env` again would set an unchanged signal and see nothing happen.
   */
  openPanel?: { panel: "machine" | "env"; at: number; resize?: boolean } | undefined;
}

export default function SessionDrawer(props: SessionDrawerProps) {
  const [open, setOpen] = createSignal(false);
  const [tab, setTab] = createSignal<Tab>("terminal");
  /**
   * When the machine tab was last asked for a resize, rather than merely
   * asked for: `/resize` wants the control, and the `⋯` menu's `Resize`
   * wants the same thing, while `Edit .env` wants neither.
   */
  const [resizeAt, setResizeAt] = createSignal<number | undefined>(undefined);

  createEffect(() => {
    const request = props.openPanel;
    if (request !== undefined) {
      setTab(request.panel);
      setOpen(true);
      if (request.resize === true) {
        setResizeAt(request.at);
      }
    }
  });

  createEffect(() => {
    function onKeyDown(event: KeyboardEvent): void {
      if (event.key === "." && (event.metaKey || event.ctrlKey)) {
        event.preventDefault();
        setOpen((was) => !was);
      }
    }
    document.addEventListener("keydown", onKeyDown);
    onCleanup(() => document.removeEventListener("keydown", onKeyDown));
  });

  return (
    <div class={cx(styles.drawer, open() && styles.drawerOpen)}>
      <button
        type="button"
        class={styles.handle}
        aria-expanded={open()}
        title={open() ? "Hide the panel (⌘.)" : "Show the panel (⌘.)"}
        aria-label={open() ? "Hide the panel" : "Show the panel"}
        onClick={() => setOpen((was) => !was)}
      >
        <Show when={open()} fallback={<PanelRightOpen size={16} aria-hidden="true" />}>
          <PanelRightClose size={16} aria-hidden="true" />
        </Show>
      </button>

      <Show when={open()}>
        <div class={styles.panel}>
          <div class={styles.tabs} role="tablist" aria-label="Session panels">
            <For each={TABS}>
              {(entry) => (
                <button
                  type="button"
                  role="tab"
                  aria-selected={tab() === entry.id}
                  class={cx(styles.tab, tab() === entry.id && styles.tabActive)}
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
                    <Match when={entry.id === "machine"}>
                      <Cpu size={13} aria-hidden="true" />
                    </Match>
                    <Match when={entry.id === "env"}>
                      <SlidersHorizontal size={13} aria-hidden="true" />
                    </Match>
                  </Switch>
                  {entry.label}
                </button>
              )}
            </For>
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
              <Match when={tab() === "machine"}>
                <MachinePanel sessionId={props.sessionId} openResize={resizeAt()} />
              </Match>
              <Match when={tab() === "env"}>
                <EnvEditor sessionId={props.sessionId} />
              </Match>
            </Switch>
          </div>
        </div>
      </Show>
    </div>
  );
}
