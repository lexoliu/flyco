/**
 * One machine the user owns, in the same card every cloud account gets.
 *
 * A host is a provider account like any other (docs/host-enrollment.md), so
 * it belongs in the compute card rather than in a list of its own — but the
 * three facts a cloud account states are the three a host does not have. It
 * has no price, because the user already bought it; no spot mode, because
 * nobody is going to evict them from their own hardware; and no invoice.
 * What it has instead is what it told flyco about itself, and whether its
 * socket is up right now.
 *
 * `Rename` and `Remove` are here rather than in a menu because they are the
 * only two things anyone does to a machine after enrolling it. A removal
 * refused by `host-has-active-sessions` is not an error: it is the control
 * plane saying how much work is still running there, and the card repeats
 * the number before offering to stop it anyway.
 */
import { Show, createSignal } from "solid-js";
import { Server } from "lucide-solid";
import { CardShell, Fact } from "./ComputeCard";
import { removeHost, renameHost, type HostView } from "../api/client";
import { formatDate } from "../lib/dates";
import { activeSessions, isBusyHost } from "../lib/hostEnrollment";
import {
  HOST_STATE_LABEL,
  hostCapacityLabel,
  hostDiskLabel,
  hostSoftwareLabel,
} from "../lib/hosts";
import { ARCHITECTURE_LABEL } from "../lib/machines";
import { relativeTime } from "../lib/relativeTime";
import styles from "./ComputeCard.module.css";

export interface HostCardProps {
  /** The machine this card is about. */
  host: HostView;
  /** Called after a rename or a removal, so the list can be reloaded. */
  onChanged: () => unknown;
  /** Whether `Rename` and `Remove` are on offer at all. */
  editable?: boolean | undefined;
}

export default function HostCard(props: HostCardProps) {
  const [renaming, setRenaming] = createSignal(false);
  const [label, setLabel] = createSignal(props.host.label);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  // The count a refused removal named, so `Remove anyway` states what it is
  // about to stop rather than asking for a blank confirmation.
  const [running, setRunning] = createSignal<number | null>(null);
  const [refused, setRefused] = createSignal(false);

  const editable = () => props.editable !== false;

  async function rename(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    const next = label().trim();
    if (next === "" || next === props.host.label) {
      setRenaming(false);
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await renameHost(props.host.id, next);
      setRenaming(false);
      await props.onChanged();
    } catch (failure) {
      setError(failure);
    } finally {
      setBusy(false);
    }
  }

  async function remove(force: boolean): Promise<void> {
    setBusy(true);
    setError(null);
    try {
      await removeHost(props.host.id, { force });
      setRefused(false);
      await props.onChanged();
    } catch (failure) {
      if (isBusyHost(failure)) {
        setRunning(activeSessions(failure));
        setRefused(true);
      } else {
        setError(failure);
      }
    } finally {
      setBusy(false);
    }
  }

  return (
    <CardShell
      mark={<Server size={16} aria-hidden="true" />}
      title={
        <Show when={renaming()} fallback={props.host.label}>
          <form class={styles.rename} onSubmit={(event) => void rename(event)}>
            <input
              class={styles.renameInput}
              aria-label={`Name for ${props.host.label}`}
              value={label()}
              onInput={(event) => setLabel(event.currentTarget.value)}
              autocomplete="off"
              spellcheck={false}
              autofocus
            />
            <button type="submit" class={styles.action} disabled={busy()}>
              Save
            </button>
            <button
              type="button"
              class={styles.action}
              onClick={() => {
                setLabel(props.host.label);
                setRenaming(false);
              }}
            >
              Cancel
            </button>
          </form>
        </Show>
      }
      meta={
        <>
          Your own machine · enrolled {formatDate(props.host.created_at_unix)}
          <Show when={props.host.state !== "online" && props.host.last_seen_unix}>
            {(seen) => <> · last seen {relativeTime(seen(), Date.now())}</>}
          </Show>
        </>
      }
      status={
        <span class={styles.state}>
          <span class={styles.dot} data-state={props.host.state} aria-hidden="true" />
          {HOST_STATE_LABEL[props.host.state]}
        </span>
      }
      actions={
        <Show when={editable() && !renaming()}>
          <button type="button" class={styles.action} onClick={() => setRenaming(true)}>
            Rename
          </button>
          <button
            type="button"
            class={styles.danger}
            disabled={busy()}
            onClick={() => void remove(false)}
          >
            Remove
          </button>
        </Show>
      }
      footer={
        <>
          <Show when={refused()}>
            <div class={styles.confirm} role="alert">
              <p class={styles.confirmLine}>
                <Show
                  when={running()}
                  fallback={<>Sessions are still running on {props.host.label}.</>}
                >
                  {(count) => (
                    <>
                      {count()} {count() === 1 ? "session is" : "sessions are"} still running on{" "}
                      {props.host.label}.
                    </>
                  )}
                </Show>{" "}
                Removing it stops their containers and keeps the disks, so the work stays on your
                hardware.
              </p>
              <div class={styles.confirmActions}>
                <button
                  type="button"
                  class={styles.danger}
                  disabled={busy()}
                  onClick={() => void remove(true)}
                >
                  Remove anyway
                </button>
                <button type="button" class={styles.action} onClick={() => setRefused(false)}>
                  Keep it
                </button>
              </div>
            </div>
          </Show>
          <Show when={error()}>
            {(failure) => (
              <p class={styles.error} role="alert">
                {failure() instanceof Error ? (failure() as Error).message : String(failure())}
              </p>
            )}
          </Show>
        </>
      }
    >
      <Fact label="Machine">
        {ARCHITECTURE_LABEL[props.host.facts.architecture]}
        {" · "}
        {hostCapacityLabel(props.host.facts)}
        {/* The card opens under the machine's own hostname, so repeating it
            here would say the same thing twice. A renamed machine is the
            case where the hostname is worth stating. */}
        <Show when={props.host.facts.hostname !== props.host.label}>
          <span class={styles.dim}>
            {" · "}
            {props.host.facts.hostname}
          </span>
        </Show>
      </Fact>

      <Fact label="Disk">
        {hostDiskLabel(props.host.facts)}
        <span class={styles.dim}> · where Podman keeps containers and volumes</span>
      </Fact>

      <Fact label="Software">{hostSoftwareLabel(props.host.facts)}</Fact>

      <Fact label="What an hour costs">
        {/* Not `$0.00`: flyco meters nothing here, and a zero would tell a
            budget it can run forever. */}
        <strong>your hardware</strong>
        <span class={styles.dim}> · a session here spends no budget</span>
      </Fact>
    </CardShell>
  );
}
