/**
 * The session's composer (docs/ux.md §9.3).
 *
 * The home composer's container and send button, with the chips a running
 * session still has a say over — the model, the machine, the budget — and
 * the context ring, handed in by the page because they are its facts. Three
 * things are added here, all of them about a conversation already in
 * progress:
 *
 * - while a turn is in flight the send button becomes `Stop`, because the
 *   only useful thing to do to a running turn is end it;
 * - `/` opens a command palette listing flyco's own session actions and
 *   then everything the running harness said it offers, so `/goal`,
 *   `/effort` and every skill of the checkout are reachable without
 *   knowing they exist;
 * - a message beginning with `!` runs in the machine's bash, and the field
 *   says so while one is being typed rather than after it is sent.
 */
import { For, type JSX, Show, createEffect, createMemo, createSignal } from "solid-js";
import { ArrowUp, Clock, Square, TerminalSquare } from "lucide-solid";
import ComposerShell from "./ComposerShell";
import type { HarnessCommand } from "../api/wire";
import { cx } from "../lib/cx";
import { BASH_PREFIX } from "../lib/shell";
import styles from "./Composer.module.css";
import sessionStyles from "./SessionComposer.module.css";

/** The session-level actions flyco runs itself (docs/ux.md §9.3). */
export type SessionCommand = "compact" | "archive" | "resize";

/** One row of the palette. */
interface PaletteEntry {
  /** The name as it is typed, without the leading slash. */
  name: string;
  /** What it does, in the harness's words or the interface's. */
  description: string;
  /**
   * What its argument is, or `null` when it takes none.
   *
   * This is what decides whether choosing a command sends it: a command
   * with nothing left to say is run on the spot, and one that expects an
   * argument is written into the field so the user can supply it.
   */
  argumentHint: string | null;
  /**
   * Who runs it. `flyco`'s own are its own product actions and never
   * reach the agent; everything else is sent as the message `/name args`,
   * which is how both harnesses take a slash command.
   */
  run: SessionCommand | "harness";
}

/**
 * Flyco's own commands, which come first and are marked as flyco's.
 *
 * They are not the harness's: `/archive` and `/resize` are things flyco
 * does to a machine, and `/compact` is routed to the control plane's own
 * compaction request rather than typed at the agent so that one browser
 * pressing it is a compaction every browser can see. Context usage is not
 * a command at all — the ring beside send is its door (docs/ux.md §9.3).
 */
const FLYCO_COMMANDS: readonly (PaletteEntry & { run: SessionCommand })[] = [
  {
    name: "compact",
    description: "Summarise the conversation to free context",
    argumentHint: null,
    run: "compact",
  },
  {
    name: "archive",
    description: "End the session and release the machine",
    argumentHint: null,
    run: "archive",
  },
  {
    name: "resize",
    description: "Move the session to another machine type",
    argumentHint: null,
    run: "resize",
  },
];

const FLYCO_NAMES: ReadonlySet<string> = new Set(FLYCO_COMMANDS.map((entry) => entry.name));

/**
 * Harness commands the palette never shows even when reported.
 *
 * `/context` is the harness's own version of the panel the usage ring
 * already opens — a second door that types a command flyco has no row for
 * is worse than none, so the harness's copy is dropped rather than shown
 * beside the ring it duplicates (docs/ux.md §9.3).
 */
const DROPPED_COMMANDS: ReadonlySet<string> = new Set(["context"]);

/**
 * What the field holds while a command is being picked, or `null` when the
 * palette has no business being open.
 *
 * A leading slash opens it, and the first space closes it again: by then
 * the command is chosen and what is being typed is its argument, so a list
 * still hanging over the field would be covering the conversation for no
 * reason.
 */
export function paletteQuery(text: string): string | null {
  if (!text.startsWith("/")) {
    return null;
  }
  const query = text.slice(1);
  return /\s/.test(query) ? null : query;
}

/** The palette's rows: flyco's own first, then the harness's own list. */
export function paletteEntries(commands: readonly HarnessCommand[]): PaletteEntry[] {
  return [
    ...FLYCO_COMMANDS,
    ...commands
      .filter(
        (command) => !FLYCO_NAMES.has(command.name) && !DROPPED_COMMANDS.has(command.name),
      )
      .map((command) => ({
        name: command.name,
        description: command.description,
        argumentHint: command.argument_hint,
        run: "harness" as const,
      })),
  ];
}

export interface SessionComposerProps {
  /** Whether a turn is running, which turns Send into Stop. */
  turnInFlight: boolean;
  /**
   * What the running harness said it offers, newest list wins.
   *
   * Empty until the session's daemon has reported one, which is why the
   * palette is useful from the first keystroke: flyco's own are
   * always there.
   */
  commands: readonly HarnessCommand[];
  /** Sends the message, verbatim — including a leading `!` or `/`. */
  onSend: (text: string) => void;
  /** Interrupts the running turn. */
  onStop: () => void;
  /** Runs one of flyco's own commands. */
  onCommand: (command: SessionCommand) => void;
  /**
   * The row under the field: the session's own chips and readouts
   * (docs/ux.md §9.3). Rendered by the page, because every one of them is
   * a fact the page holds — the model, the machine, the budget, the
   * context — and a composer that fetched them itself would be a second
   * copy of the session.
   */
  controls?: JSX.Element | undefined;
  /**
   * When a message typed now will not be delivered now, and why.
   *
   * The one thing a composer must never do is take a message and say
   * nothing about what happens to it. A session waiting out a spent plan
   * window still takes messages — the control plane holds the message
   * against the pause and sends it when the window turns over — and this is
   * where it says so, in the same place a `!` says it runs in the machine's
   * bash. Absent while the session is simply running.
   */
  deferred?: string | undefined;
}

export default function SessionComposer(props: SessionComposerProps) {
  const [text, setText] = createSignal("");
  const [highlighted, setHighlighted] = createSignal(0);
  /**
   * Whether Escape has closed the palette for what is currently typed.
   *
   * Escape means "stop offering", not "delete what I typed", so the field
   * keeps its text and only the list goes away. The next keystroke clears
   * this: the user has started choosing again.
   */
  const [dismissed, setDismissed] = createSignal(false);
  let field: HTMLTextAreaElement | undefined;
  let list: HTMLUListElement | undefined;

  /** A message beginning with `!` is a shell command, not a prompt. */
  const isBash = createMemo(() => text().startsWith(BASH_PREFIX));

  const entries = createMemo(() => paletteEntries(props.commands));

  /** The palette's rows, filtered by whatever has been typed after the slash. */
  const matches = createMemo(() => {
    const query = paletteQuery(text());
    if (query === null) {
      return [];
    }
    const wanted = query.toLowerCase();
    return entries().filter((entry) => entry.name.toLowerCase().startsWith(wanted));
  });

  const paletteOpen = createMemo(() => !dismissed() && matches().length > 0);

  // A checkout with ninety skills is a list taller than the window, so the
  // row the arrow keys are on has to be brought to where the eyes are.
  createEffect(() => {
    const index = highlighted();
    if (!paletteOpen()) {
      return;
    }
    list?.children[index]?.scrollIntoView({ block: "nearest" });
  });

  function clear(): void {
    setText("");
    setHighlighted(0);
    setDismissed(false);
  }

  /**
   * Runs one row, or writes it into the field when it wants an argument.
   *
   * The split is the whole point of `argumentHint`: `/archive` has nothing
   * left to ask, so choosing it is sending it, while `/goal` without its
   * condition would be a command that means nothing.
   */
  function choose(entry: PaletteEntry): void {
    if (entry.argumentHint !== null) {
      setText(`/${entry.name} `);
      setHighlighted(0);
      setDismissed(false);
      field?.focus();
      return;
    }
    clear();
    if (entry.run === "harness") {
      props.onSend(`/${entry.name}`);
    } else {
      props.onCommand(entry.run);
    }
    field?.focus();
  }

  function send(): void {
    const message = text().trim();
    if (message === "") {
      return;
    }
    // A typed-out command flyco runs itself is the same action as picking
    // it from the palette; a user who has typed the whole word should not
    // have to press a different key to get the same result. Everything
    // else goes to the agent verbatim, slash and arguments included —
    // which is exactly how both harnesses take a slash command.
    const own = FLYCO_COMMANDS.find((entry) => `/${entry.name}` === message.toLowerCase());
    if (own !== undefined) {
      clear();
      props.onCommand(own.run);
      field?.focus();
      return;
    }
    clear();
    props.onSend(message);
  }

  function onInput(value: string): void {
    setText(value);
    setHighlighted(0);
    setDismissed(false);
  }

  /** Palette navigation. Returns `true` when the key was the palette's. */
  function onKeyDown(event: KeyboardEvent): boolean {
    if (!paletteOpen()) {
      return false;
    }
    const rows = matches();
    if (event.key === "Escape") {
      event.preventDefault();
      setDismissed(true);
      setHighlighted(0);
      return true;
    }
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      const step = event.key === "ArrowDown" ? 1 : rows.length - 1;
      setHighlighted((index) => (index + step) % rows.length);
      return true;
    }
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      const entry = rows[highlighted()];
      if (entry !== undefined) {
        choose(entry);
      }
      return true;
    }
    if (event.key === "Tab") {
      event.preventDefault();
      const entry = rows[highlighted()];
      if (entry !== undefined) {
        setText(`/${entry.name}`);
      }
      return true;
    }
    return false;
  }

  return (
    <ComposerShell
      value={text()}
      onInput={onInput}
      onSubmit={send}
      placeholder="Reply, / for a command, ! for the shell"
      label="Message the agent"
      submitOn="enter"
      controls={props.controls}
      onKeyDown={onKeyDown}
      ref={(element) => {
        field = element;
      }}
      overlay={
        <Show when={paletteOpen()}>
          <ul
            class={sessionStyles.palette}
            role="listbox"
            aria-label="Session commands"
            ref={(element) => {
              list = element;
            }}
          >
            <For each={matches()}>
              {(entry, index) => (
                <li>
                  <button
                    type="button"
                    role="option"
                    aria-selected={index() === highlighted()}
                    class={cx(
                      sessionStyles.command,
                      index() === highlighted() && sessionStyles.commandHighlighted,
                    )}
                    onMouseEnter={() => setHighlighted(index())}
                    onClick={() => choose(entry)}
                  >
                    <span class={sessionStyles.commandName}>/{entry.name}</span>
                    <Show when={entry.argumentHint}>
                      {(hint) => <span class={sessionStyles.commandHint}>{hint()}</span>}
                    </Show>
                    <span class={sessionStyles.commandDescription}>{entry.description}</span>
                    <Show when={entry.run !== "harness"}>
                      <span class={sessionStyles.commandOwner}>flyco</span>
                    </Show>
                  </button>
                </li>
              )}
            </For>
          </ul>
        </Show>
      }
      action={
        <Show
          when={props.turnInFlight}
          fallback={
            <button
              type="button"
              class={styles.send}
              disabled={text().trim() === ""}
              title="Send"
              aria-label="Send"
              onClick={send}
            >
              <ArrowUp size={16} aria-hidden="true" />
            </button>
          }
        >
          <button
            type="button"
            class={styles.stop}
            title="Stop the turn"
            aria-label="Stop the turn"
            onClick={() => props.onStop()}
          >
            <Square size={12} fill="currentColor" aria-hidden="true" />
          </button>
        </Show>
      }
    >
      <Show
        when={isBash()}
        fallback={
          // The shell hint wins while a `!` is being typed: a command runs on
          // the machine there and then, whatever the plan's limits are doing,
          // so saying it would wait for a reset would be wrong.
          <Show when={props.deferred}>
            {(note) => (
              <p class={sessionStyles.hint}>
                <Clock size={13} aria-hidden="true" />
                {note()}
              </p>
            )}
          </Show>
        }
      >
        <p class={sessionStyles.hint}>
          <TerminalSquare size={13} aria-hidden="true" />
          Runs in the machine's bash
        </p>
      </Show>
    </ComposerShell>
  );
}
