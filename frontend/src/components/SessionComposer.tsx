/**
 * The session's composer (docs/ux.md §9.3).
 *
 * The home composer's container and send button, without the chips —
 * everything a chip decides was decided when the session was created. Three
 * things are added, all of them about a conversation already in progress:
 *
 * - while a turn is in flight the send button becomes `Stop`, because the
 *   only useful thing to do to a running turn is end it;
 * - `/` at the start of an empty field opens a command palette, so the
 *   three session-level actions are reachable without a menu;
 * - a message beginning with `!` runs in the machine's bash, and the field
 *   says so while one is being typed rather than after it is sent.
 */
import { For, Show, createMemo, createSignal } from "solid-js";
import { ArrowUp, Square, TerminalSquare } from "lucide-solid";
import ComposerShell from "./ComposerShell";
import { cx } from "../lib/cx";
import styles from "./Composer.module.css";
import sessionStyles from "./SessionComposer.module.css";

/** What `/` opens. One entry per session-level action (docs/ux.md §9.3). */
export type SessionCommand = "compact" | "archive" | "resize";

interface CommandEntry {
  command: SessionCommand;
  /** What the user types, including the slash. */
  typed: string;
  /** What it does, in the interface's voice. */
  description: string;
}

const COMMANDS: readonly CommandEntry[] = [
  {
    command: "compact",
    typed: "/compact",
    description: "Summarise the conversation to free context",
  },
  { command: "archive", typed: "/archive", description: "End the session and release the machine" },
  { command: "resize", typed: "/resize", description: "Move the session to another machine type" },
];

/** The prefix that sends a message straight to the machine's shell. */
const BASH_PREFIX = "!";

export interface SessionComposerProps {
  /** Whether a turn is running, which turns Send into Stop. */
  turnInFlight: boolean;
  /** Whether the composer accepts input at all (an archived session does not). */
  disabled?: boolean | undefined;
  /** Sends the message, verbatim — including a leading `!`. */
  onSend: (text: string) => void;
  /** Interrupts the running turn. */
  onStop: () => void;
  /** Runs one palette command. */
  onCommand: (command: SessionCommand) => void;
}

export default function SessionComposer(props: SessionComposerProps) {
  const [text, setText] = createSignal("");
  const [paletteOpen, setPaletteOpen] = createSignal(false);
  const [highlighted, setHighlighted] = createSignal(0);
  let field: HTMLTextAreaElement | undefined;

  /** A message beginning with `!` is a shell command, not a prompt. */
  const isBash = createMemo(() => text().startsWith(BASH_PREFIX));

  /** The palette's entries, filtered by whatever has been typed after the slash. */
  const matches = createMemo(() => {
    const typed = text().trim().toLowerCase();
    if (!typed.startsWith("/")) {
      return [...COMMANDS];
    }
    return COMMANDS.filter((entry) => entry.typed.startsWith(typed));
  });

  function closePalette(): void {
    setPaletteOpen(false);
    setHighlighted(0);
  }

  function run(entry: CommandEntry): void {
    setText("");
    closePalette();
    props.onCommand(entry.command);
    field?.focus();
  }

  function send(): void {
    const message = text().trim();
    if (message === "" || props.disabled === true) {
      return;
    }
    // A typed-out command is the same action as picking it from the
    // palette; a user who has typed the whole word should not have to
    // press a different key to get the same result.
    const entry = COMMANDS.find((candidate) => candidate.typed === message.toLowerCase());
    if (entry !== undefined) {
      run(entry);
      return;
    }
    setText("");
    closePalette();
    props.onSend(message);
  }

  function onInput(value: string): void {
    setText(value);
    // The palette follows what is in the field: it opens on a leading
    // slash and closes as soon as the message stops being a command.
    if (value.startsWith("/")) {
      setPaletteOpen(true);
      setHighlighted(0);
    } else {
      closePalette();
    }
  }

  /** Palette navigation. Returns `true` when the key was the palette's. */
  function onKeyDown(event: KeyboardEvent): boolean {
    if (!paletteOpen()) {
      return false;
    }
    const entries = matches();
    if (event.key === "Escape") {
      event.preventDefault();
      closePalette();
      return true;
    }
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      if (entries.length > 0) {
        const step = event.key === "ArrowDown" ? 1 : entries.length - 1;
        setHighlighted((index) => (index + step) % entries.length);
      }
      return true;
    }
    if (event.key === "Enter" && !event.shiftKey && entries.length > 0) {
      event.preventDefault();
      const entry = entries[highlighted()];
      if (entry !== undefined) {
        run(entry);
      }
      return true;
    }
    if (event.key === "Tab" && entries.length > 0) {
      event.preventDefault();
      const entry = entries[highlighted()];
      if (entry !== undefined) {
        setText(entry.typed);
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
      placeholder="Message the agent, / for commands, ! to run a shell command"
      label="Message the agent"
      submitOn="enter"
      disabled={props.disabled ?? false}
      onKeyDown={onKeyDown}
      ref={(element) => {
        field = element;
      }}
      overlay={
        <Show when={paletteOpen() && matches().length > 0}>
          <ul class={sessionStyles.palette} role="listbox" aria-label="Session commands">
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
                    onClick={() => run(entry)}
                  >
                    <span class={sessionStyles.commandName}>{entry.typed}</span>
                    <span class={sessionStyles.commandDescription}>{entry.description}</span>
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
              disabled={text().trim() === "" || props.disabled === true}
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
      <Show when={isBash()}>
        <p class={sessionStyles.hint}>
          <TerminalSquare size={13} aria-hidden="true" />
          Runs in the machine's bash
        </p>
      </Show>
    </ComposerShell>
  );
}
