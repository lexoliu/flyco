/**
 * The session header (docs/ux.md §9.1).
 *
 * One row: the title, editable in place; what the session works on; the
 * status it reads as; the machine it runs on; how much of its budget and
 * its context window are gone; and the two actions — `Archive` and a `⋯`
 * menu for everything that is not an everyday move.
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
  Play,
  Scaling,
  SlidersHorizontal,
  Square,
} from "lucide-solid";
import Popover from "./Popover";
import Ring from "./Ring";
import type { MachineView, SessionDetail } from "../api/client";
import type { ConnectionState } from "../api/relay";
import { cx } from "../lib/cx";
import { formatUsd } from "../lib/money";
import { PROVIDER_LABEL } from "../lib/providers";
import type { StatusView } from "../lib/status";
import styles from "./SessionHeader.module.css";

/** `Standard_B2s · $0.04/hr · spot`, or nothing while the machine is unknown. */
function machineChip(machine: MachineView | undefined): string | null {
  if (machine === undefined) {
    return null;
  }
  const parts = [machine.spec.machine_type];
  if (machine.hourly !== null && machine.hourly !== undefined) {
    parts.push(`${formatUsd(machine.hourly)}/hr`);
  }
  if (machine.spot) {
    parts.push("spot");
  }
  return parts.join(" · ");
}

/**
 * What a ring reads while its number is not known.
 *
 * A dash, not a sentence: "not loaded" and "not reported" describe the
 * plumbing, and a header that explains its plumbing is a header that looks
 * broken. The ring's label still names what the dash stands for.
 */
const UNKNOWN_READOUT = "—";

/** `41k / 200k`, because a context window is read in thousands or not at all. */
function tokens(count: number): string {
  return count >= 1000 ? `${Math.round(count / 1000)}k` : `${count}`;
}

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
};

export interface SessionHeaderProps {
  session: SessionDetail | undefined;
  /** The id, for the `Copy session id` action; never shown as a title. */
  sessionId: string;
  status: StatusView;
  /** The relay socket's state, shown only while it is not carrying events. */
  connection: ConnectionState;
  machine: MachineView | undefined;
  /** Budget spent and allowed, in whole dollars. */
  budgetSpentUsd: number | undefined;
  budgetLimitUsd: number | undefined;
  /** Context window used and available, in tokens. */
  contextUsed: number | undefined;
  contextSize: number | undefined;
  /** Renames the session. */
  onRename: (title: string) => void;
  onArchive: () => void;
  archiving: boolean;
  onStartMachine: () => void;
  onStopMachine: () => void;
  /** Opens the drawer on a given tab, for the menu items that live there. */
  onOpenPanel: (panel: "machine" | "env") => void;
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

      <div class={styles.readouts}>
        <span
          class={cx(styles.status, props.status.breathing && styles.breathing)}
          data-tone={props.status.tone}
        >
          <span class={styles.statusDot} aria-hidden="true" />
          {props.status.label}
          <Show when={props.status.detail}>
            {(detail) => <span class={styles.statusDetail}>· {detail()}</span>}
          </Show>
        </span>

        <Show when={CONNECTION_LABEL[props.connection]}>
          {(label) => <span class={styles.connection}>{label()}</span>}
        </Show>

        <Show when={machineChip(props.machine)}>
          {(chip) => <span class={styles.machine}>{chip()}</span>}
        </Show>

        <Ring
          label="Budget"
          value={props.budgetSpentUsd}
          total={props.budgetLimitUsd}
          readout={
            props.budgetSpentUsd === undefined || props.budgetLimitUsd === undefined
              ? UNKNOWN_READOUT
              : `$${props.budgetSpentUsd.toFixed(2)} / $${props.budgetLimitUsd.toFixed(0)}`
          }
        />
        <Ring
          label="Context"
          value={props.contextUsed}
          total={props.contextSize}
          readout={
            props.contextUsed === undefined || props.contextSize === undefined
              ? UNKNOWN_READOUT
              : `${tokens(props.contextUsed)} / ${tokens(props.contextSize)}`
          }
        />
      </div>

      <div class={styles.actions}>
        <Show when={props.session?.state !== "archived"}>
          <button
            type="button"
            class={styles.action}
            disabled={props.archiving}
            onClick={() => props.onArchive()}
          >
            <Archive size={13} aria-hidden="true" />
            {props.archiving ? "Archiving…" : "Archive"}
          </button>
        </Show>

        <Popover
          label="More session actions"
          align="end"
          trigger={(attrs) => (
            <button {...attrs} type="button" class={styles.iconAction} aria-label="More actions">
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
                    props.onOpenPanel("machine");
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
                    props.onOpenPanel("env");
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
                    {machine().state}
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
